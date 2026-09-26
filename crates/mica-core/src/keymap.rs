//! 快捷键表(D15):Windows Terminal 兼容默认键位 + `keybind` 配置化。
//! 纯逻辑:触发器/动作的解析与查表,不含任何执行——执行是壳层的职责。

use crate::input::{Key, Mods};

/// 触发键:导航/编辑键用现成 Key,字母/数字是 KEYDOWN 的 VK 空间。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TriggerKey {
    Key(Key),
    Letter(char),
    Digit(u8),
    Insert,
}

/// 完整触发器 = 键 + 修饰组合。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Trigger {
    pub mods: Mods,
    pub key: TriggerKey,
}

impl Trigger {
    pub fn new(mods: Mods, key: TriggerKey) -> Self {
        Self { mods, key }
    }
}

/// 终端外语义动作。分屏动作(M2b)在此占位,键位可先绑定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    GotoTab(u8),
    Copy,
    Paste,
    /// 正值向历史方向滚 N 行,负值向底部。
    ScrollLine(i32),
    /// 正值向历史方向滚一屏,负值向底部。
    ScrollPage(i32),
    ScrollTop,
    ScrollBottom,
    SplitRight,
    SplitDown,
    ClosePane,
}

/// 键位表:用户条目在前(优先),默认表垫底。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Keymap {
    binds: Vec<(Trigger, Action)>,
}

impl Keymap {
    /// WT 兼容默认表(D15):目标用户是 WT 迁移者,肌肉记忆零成本。
    pub fn wt_default() -> Self {
        let m = |ctrl: bool, shift: bool, alt: bool| Mods { shift, alt, ctrl };
        let mut binds = Vec::new();
        let mut push = |mods: Mods, key: TriggerKey, action: Action| {
            binds.push((Trigger::new(mods, key), action))
        };
        // 标签
        push(
            m(true, true, false),
            TriggerKey::Letter('T'),
            Action::NewTab,
        );
        push(
            m(true, true, false),
            TriggerKey::Letter('W'),
            Action::CloseTab,
        );
        push(
            m(true, false, false),
            TriggerKey::Key(Key::Tab),
            Action::NextTab,
        );
        push(
            m(true, true, false),
            TriggerKey::Key(Key::Tab),
            Action::PrevTab,
        );
        for d in 1..=9u8 {
            push(
                m(true, true, false),
                TriggerKey::Digit(d),
                Action::GotoTab(d),
            );
        }
        // 复制粘贴(WT 的 Ctrl+Shift+C/V;裸 Ctrl+C/V 的智能语义在壳层,不进表)
        push(m(true, true, false), TriggerKey::Letter('C'), Action::Copy);
        push(m(true, true, false), TriggerKey::Letter('V'), Action::Paste);
        push(m(true, false, false), TriggerKey::Insert, Action::Copy);
        push(m(false, true, false), TriggerKey::Insert, Action::Paste);
        // 滚动:正方向 = 历史
        push(
            m(false, true, false),
            TriggerKey::Key(Key::PageUp),
            Action::ScrollPage(1),
        );
        push(
            m(false, true, false),
            TriggerKey::Key(Key::PageDown),
            Action::ScrollPage(-1),
        );
        push(
            m(true, true, false),
            TriggerKey::Key(Key::Home),
            Action::ScrollTop,
        );
        push(
            m(true, true, false),
            TriggerKey::Key(Key::End),
            Action::ScrollBottom,
        );
        push(
            m(true, true, false),
            TriggerKey::Key(Key::Up),
            Action::ScrollLine(1),
        );
        push(
            m(true, true, false),
            TriggerKey::Key(Key::Down),
            Action::ScrollLine(-1),
        );
        Self { binds }
    }

    /// 清空(配置 `keybind = clear` 后只保留用户条目)。
    pub fn clear(&mut self) {
        self.binds.clear();
    }

    /// 追加一条(后值覆盖同触发器的旧绑定)。
    pub fn bind(&mut self, trigger: Trigger, action: Action) {
        self.binds.retain(|(t, _)| *t != trigger);
        self.binds.push((trigger, action));
    }

    /// 查表:用户区(前)优先于默认区(后)——构造时保证顺序。
    pub fn lookup(&self, mods: Mods, key: TriggerKey) -> Option<Action> {
        // 忽略触发器未声明的修饰(如未按 shift 的 capslock 噪声不做声):
        // 精确匹配 mods,键唯一即可
        self.binds
            .iter()
            .find(|(t, _)| t.mods == mods && t.key == key)
            .map(|(_, a)| *a)
    }
}

/// 解析触发器语法:`ctrl+shift+t`、`shift+pageup`、`alt+d`、`f5`。
/// 段以 `+` 连接,末段是键名(不区分大小写)。
pub fn parse_trigger(s: &str) -> Option<Trigger> {
    let mut mods = Mods::NONE;
    let mut parts = s.split('+').map(str::trim).collect::<Vec<_>>();
    let key_name = parts.pop()?.to_lowercase();
    for part in parts {
        match part.to_lowercase().as_str() {
            "ctrl" => mods.ctrl = true,
            "shift" => mods.shift = true,
            "alt" => mods.alt = true,
            _ => return None,
        }
    }
    let key = match key_name.as_str() {
        "enter" => TriggerKey::Key(Key::Enter),
        "backspace" => TriggerKey::Key(Key::Backspace),
        "tab" => TriggerKey::Key(Key::Tab),
        "esc" | "escape" => TriggerKey::Key(Key::Escape),
        "up" => TriggerKey::Key(Key::Up),
        "down" => TriggerKey::Key(Key::Down),
        "left" => TriggerKey::Key(Key::Left),
        "right" => TriggerKey::Key(Key::Right),
        "home" => TriggerKey::Key(Key::Home),
        "end" => TriggerKey::Key(Key::End),
        "delete" => TriggerKey::Key(Key::Delete),
        "pageup" => TriggerKey::Key(Key::PageUp),
        "pagedown" => TriggerKey::Key(Key::PageDown),
        "insert" => TriggerKey::Insert,
        d if d.len() == 1 && d.chars().next().is_some_and(|c| c.is_ascii_digit()) => {
            let digit = key_name.parse::<u8>().ok()?;
            // 0 不是合法数字触发键(与 9 上限一致)
            (1..=9)
                .contains(&digit)
                .then_some(TriggerKey::Digit(digit))?
        }
        name => {
            let mut chars = name.chars();
            let c = chars.next()?;
            // 单字母:字母键(大小写不敏感)
            if c.is_ascii_alphabetic() && chars.next().is_none() {
                TriggerKey::Letter(c.to_ascii_uppercase())
            } else {
                return None;
            }
        }
    };
    Some(Trigger::new(mods, key))
}

/// 解析动作名(snake_case;goto_tab_<n> 特化)。
pub fn parse_action(s: &str) -> Option<Action> {
    let name = s.trim();
    match name {
        "new_tab" => Some(Action::NewTab),
        "close_tab" => Some(Action::CloseTab),
        "next_tab" => Some(Action::NextTab),
        "prev_tab" => Some(Action::PrevTab),
        "copy" => Some(Action::Copy),
        "paste" => Some(Action::Paste),
        "scroll_top" => Some(Action::ScrollTop),
        "scroll_bottom" => Some(Action::ScrollBottom),
        "scroll_line_up" => Some(Action::ScrollLine(1)),
        "scroll_line_down" => Some(Action::ScrollLine(-1)),
        "scroll_page_up" => Some(Action::ScrollPage(1)),
        "scroll_page_down" => Some(Action::ScrollPage(-1)),
        "split_right" => Some(Action::SplitRight),
        "split_down" => Some(Action::SplitDown),
        "close_pane" => Some(Action::ClosePane),
        _ => {
            // goto_tab_1 ..= goto_tab_9
            let n = name.strip_prefix("goto_tab_")?;
            let n: u8 = n.parse().ok()?;
            (1..=9).contains(&n).then_some(Action::GotoTab(n))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wt_default_covers_core_combos() {
        let km = Keymap::wt_default();
        let m = |c: bool, s: bool, a: bool| Mods {
            shift: s,
            alt: a,
            ctrl: c,
        };
        assert_eq!(
            km.lookup(m(true, true, false), TriggerKey::Letter('T')),
            Some(Action::NewTab)
        );
        assert_eq!(
            km.lookup(m(true, false, false), TriggerKey::Key(Key::Tab)),
            Some(Action::NextTab)
        );
        assert_eq!(
            km.lookup(m(true, true, false), TriggerKey::Digit(3)),
            Some(Action::GotoTab(3))
        );
        assert_eq!(
            km.lookup(m(false, true, false), TriggerKey::Key(Key::PageUp)),
            Some(Action::ScrollPage(1))
        );
        assert_eq!(
            km.lookup(m(false, false, false), TriggerKey::Letter('T')),
            None
        );
    }

    #[test]
    fn bind_overrides_same_trigger() {
        let mut km = Keymap::wt_default();
        let t = parse_trigger("ctrl+shift+t").unwrap();
        km.bind(t, Action::SplitRight);
        assert_eq!(
            km.lookup(
                Mods {
                    ctrl: true,
                    shift: true,
                    alt: false
                },
                t.key
            ),
            Some(Action::SplitRight)
        );
    }

    #[test]
    fn trigger_parsing_roundtrip() {
        assert_eq!(
            parse_trigger("ctrl+shift+t"),
            Some(Trigger::new(
                Mods {
                    ctrl: true,
                    shift: true,
                    alt: false
                },
                TriggerKey::Letter('T')
            ))
        );
        assert_eq!(
            parse_trigger("SHIFT+PageUp"),
            Some(Trigger::new(
                Mods {
                    ctrl: false,
                    shift: true,
                    alt: false
                },
                TriggerKey::Key(Key::PageUp)
            ))
        );
        assert!(parse_trigger("alt+d").is_some());
        assert_eq!(parse_trigger("hyper+t"), None, "未知修饰符");
        assert_eq!(parse_trigger("ctrl+xyzzy"), None, "未知键名");
    }

    #[test]
    fn action_parsing() {
        assert_eq!(parse_action("new_tab"), Some(Action::NewTab));
        assert_eq!(parse_action("goto_tab_7"), Some(Action::GotoTab(7)));
        assert_eq!(parse_action("goto_tab_0"), None, "越界");
        assert_eq!(parse_action("goto_tab_x"), None);
        assert_eq!(parse_action("nope"), None);
    }
}
