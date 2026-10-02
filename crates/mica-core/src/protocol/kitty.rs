//! kitty keyboard protocol 编码器(M4a/D27):key+mods+kind → CSI-u 序列。
//!
//! 协议事实(见 plan §Global Constraints):
//! - `CSI key-code[:modifiers[:event-type]] [;text] u`;键码 = unshifted
//!   Unicode 或功能键常量;modifiers = 1+shift(1)+alt(2)+ctrl(4)+super(8)
//! - event-type 仅 REPORT_EVENT_TYPES 开启时发(repeat 亦然);all-keys-as-esc
//!   开启时普通字符也编码,否则可打印键回 None(调用方走 legacy 文本路径)
//! - associated text 仅 press 携带(unshifted 原则:shift+1 的 text 是 "!")
//!   且 ctrl/alt 组合不携带(各终端语义不一,保守省略)

use crate::input::{Key, Mods};

/// 输入事件类别(kitty event-type:1 press/2 repeat/3 release)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Press,
    Repeat,
    Release,
}

/// TermMode 的 kitty flag 位值(与上游 mod.rs:75-85 锁死,T2 有运行时对齐测试)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KittyFlags(pub u32);

impl KittyFlags {
    pub const NONE: Self = Self(0);
    pub const DISAMBIGUATE: Self = Self(1 << 18);
    pub const REPORT_EVENT_TYPES: Self = Self(1 << 19);
    pub const ALL_KEYS_AS_ESC: Self = Self(1 << 21);
    pub const REPORT_ASSOCIATED_TEXT: Self = Self(1 << 22);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// 任一 kitty 位开启(路由判定:非零才考虑 kitty 编码)。
    pub fn any(self) -> bool {
        self.0 != 0
    }

    /// disambiguate 开启时,Esc/Enter/Tab/Backspace 等产生歧义的键走编码
    /// (普通可打印字符仍 None)。
    fn disambiguate(self) -> bool {
        self.contains(Self::DISAMBIGUATE) || self.contains(Self::ALL_KEYS_AS_ESC)
    }
}

/// 功能键 keysym 常量(kitty 协议 functional 编号)。
/// Key::Grave 不在其中——反引号是可打印字符,走 WM_CHAR 文本路径,
/// kitty 协议下同样不发编码序列(除非 all-keys,M4a 不扩 Key 枚举)。
fn functional_keysym(key: Key) -> Option<u32> {
    Some(match key {
        Key::Escape => 27,
        Key::Enter => 13,
        Key::Tab => 9,
        Key::Backspace => 127,
        Key::Up => 57344,
        Key::Down => 57345,
        Key::Right => 57346,
        Key::Left => 57347,
        Key::Home => 57348,
        Key::End => 57349,
        Key::PageUp => 57350,
        Key::PageDown => 57351,
        Key::Delete => 57363,
        // 可打印字符(反引号)走 WM_CHAR 文本路径,协议编码不覆盖
        Key::Grave => return None,
    })
}

/// 修饰参数位:kitty = 1 + shift + alt*2 + ctrl*4 + super*8。
fn kitty_mods(mods: Mods) -> u32 {
    1 + u32::from(mods.shift)
        + 2 * u32::from(mods.alt)
        + 4 * u32::from(mods.ctrl)
        + 8 * u32::from(mods.win)
}

/// key + mods + kind → kitty CSI-u 序列。
/// None = 当前 flags 下不走 kitty(调用方回 legacy `input::encode`/文本路径)。
pub fn kitty_encode(key: Key, mods: Mods, kind: EventKind, flags: KittyFlags) -> Option<Vec<u8>> {
    // release/repeat 仅在 REPORT_EVENT_TYPES 开启时上报
    if !flags.contains(KittyFlags::REPORT_EVENT_TYPES) && !matches!(kind, EventKind::Press) {
        return None;
    }
    // None(可打印字符如 Grave)走 WM_CHAR 文本路径,不在此编码
    let code = functional_keysym(key)?;
    // 功能键在 disambiguate 任一形态开启时编码;全关时功能键本来就不歧义
    // (legacy CSI 序列已覆盖),回 None。
    if !flags.disambiguate() {
        return None;
    }
    let m = kitty_mods(mods);
    let mut seq = format!("\x1b[{code}:{m}").into_bytes();
    if flags.contains(KittyFlags::REPORT_EVENT_TYPES) {
        let et = match kind {
            EventKind::Press => 1,
            EventKind::Repeat => 2,
            EventKind::Release => 3,
        };
        seq.extend_from_slice(format!(":{et}").as_bytes());
    }
    // 功能键无 associated text(协议:文本仅与 unicode keycode 同发)
    seq.extend_from_slice(b"u");
    Some(seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: KittyFlags = KittyFlags(0);
    fn flags(bits: u32) -> KittyFlags {
        KittyFlags(bits)
    }

    fn plain() -> Mods {
        Mods::NONE
    }

    fn ctrl() -> Mods {
        Mods {
            ctrl: true,
            ..Mods::NONE
        }
    }

    fn shift_ctrl() -> Mods {
        Mods {
            shift: true,
            ctrl: true,
            ..Mods::NONE
        }
    }

    #[test]
    fn flags_off_yields_none() {
        assert_eq!(kitty_encode(Key::Up, plain(), EventKind::Press, ALL), None);
        assert_eq!(
            kitty_encode(Key::Up, plain(), EventKind::Press, flags(1 << 22)),
            None,
            "associated-text 单开不算 disambiguate"
        );
    }

    #[test]
    fn disambiguate_encodes_functional_press() {
        assert_eq!(
            kitty_encode(Key::Up, plain(), EventKind::Press, KittyFlags::DISAMBIGUATE),
            Some(b"\x1b[57344:1u".to_vec())
        );
    }

    #[test]
    fn modifiers_sum() {
        assert_eq!(
            kitty_encode(
                Key::Right,
                ctrl(),
                EventKind::Press,
                KittyFlags::DISAMBIGUATE
            ),
            Some(b"\x1b[57346:5u".to_vec()),
            "ctrl = 1+4"
        );
        assert_eq!(
            kitty_encode(
                Key::Left,
                shift_ctrl(),
                EventKind::Press,
                KittyFlags::DISAMBIGUATE
            ),
            Some(b"\x1b[57347:6u".to_vec()),
            "shift+ctrl = 1+1+4"
        );
    }

    #[test]
    fn win_modifier_counts() {
        let mods = Mods {
            win: true,
            ..Mods::NONE
        };
        assert_eq!(
            kitty_encode(Key::Up, mods, EventKind::Press, KittyFlags::DISAMBIGUATE),
            Some(b"\x1b[57344:9u".to_vec()),
            "super = 1+8"
        );
    }

    #[test]
    fn event_types_gate_release_and_repeat() {
        assert_eq!(
            kitty_encode(
                Key::Up,
                plain(),
                EventKind::Release,
                KittyFlags::DISAMBIGUATE
            ),
            None,
            "release 仅 REPORT_EVENT_TYPES 开启时发"
        );
        assert_eq!(
            kitty_encode(
                Key::Up,
                plain(),
                EventKind::Release,
                KittyFlags(1 << 18 | 1 << 19)
            ),
            Some(b"\x1b[57344:1:3u".to_vec())
        );
        assert_eq!(
            kitty_encode(
                Key::Down,
                plain(),
                EventKind::Repeat,
                KittyFlags(1 << 18 | 1 << 19)
            ),
            Some(b"\x1b[57345:1:2u".to_vec()),
            "repeat 同门(编码还需 disambiguate 路由)"
        );
    }

    #[test]
    fn all_keys_as_esc_also_disambiguates() {
        assert_eq!(
            kitty_encode(
                Key::Enter,
                plain(),
                EventKind::Press,
                KittyFlags::ALL_KEYS_AS_ESC
            ),
            Some(b"\x1b[13:1u".to_vec())
        );
    }

    #[test]
    fn every_key_variant_encodes() {
        for key in [
            Key::Enter,
            Key::Backspace,
            Key::Tab,
            Key::Escape,
            Key::Up,
            Key::Down,
            Key::Left,
            Key::Right,
            Key::Home,
            Key::End,
            Key::Delete,
            Key::PageUp,
            Key::PageDown,
        ] {
            assert!(
                kitty_encode(key, plain(), EventKind::Press, KittyFlags::DISAMBIGUATE).is_some(),
                "{key:?} 应可编码"
            );
        }
    }
}
