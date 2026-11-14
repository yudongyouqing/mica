//! 快捷键表(D15):Windows Terminal 兼容默认键位 + `keybind` 配置化。
//! 纯逻辑:触发器/动作的解析与查表,不含任何执行——执行是壳层的职责。

use crate::input::{Key, Mods};

/// #79 应用层修饰键重映射(RemapKey → Mods 修饰位):encode 前转换,
/// keymap 动作判定不受影响(动作看原始修饰)。
/// `key-remap = ctrl=super` 语义:物理 Ctrl 按下时,发给 pty 的编码
/// 带 Super 位而非 Ctrl 位
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyRemap {
    /// 源修饰(按下的)
    pub from: RemapMod,
    /// 目标修饰(编码进 pty 的)
    pub to: RemapMod,
}

/// 可重映射的修饰位(Win 键在 Windows 消息里不是独立 VK 修饰,以
/// mods.win 标志承载;M3b 同款)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemapMod {
    Shift,
    Alt,
    Ctrl,
    Win,
}

impl RemapMod {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "shift" => Some(Self::Shift),
            "alt" | "option" => Some(Self::Alt),
            "ctrl" | "control" => Some(Self::Ctrl),
            "win" | "super" | "cmd" | "meta" => Some(Self::Win),
            _ => None,
        }
    }
}

impl KeyRemap {
    /// 应用重映射:输入 Mods → 输出编码用 Mods(重映射源/目标翻转)
    pub fn apply(&self, mods: Mods) -> Mods {
        let get = |m: Mods, r: RemapMod| match r {
            RemapMod::Shift => m.shift,
            RemapMod::Alt => m.alt,
            RemapMod::Ctrl => m.ctrl,
            RemapMod::Win => m.win,
        };
        let set = |m: &mut Mods, r: RemapMod, v: bool| match r {
            RemapMod::Shift => m.shift = v,
            RemapMod::Alt => m.alt = v,
            RemapMod::Ctrl => m.ctrl = v,
            RemapMod::Win => m.win = v,
        };
        let from_down = get(mods, self.from);
        let to_down = get(mods, self.to);
        let mut out = mods;
        set(&mut out, self.from, to_down);
        set(&mut out, self.to, from_down);
        out
    }

    /// 解析单条 `a=b`(from=to)
    pub fn parse_pair(s: &str) -> Option<Self> {
        let (from, to) = s.split_once('=')?;
        Some(Self {
            from: RemapMod::parse(from)?,
            to: RemapMod::parse(to)?,
        })
    }
}

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
    /// 新建独立窗口(2026-10-12):新线程 + 新 HWND,同 wndproc 同管线;
    /// 进程活到最后一窗关闭
    NewWindow,
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
    /// 焦点跳邻 pane(方向)。Alt+方向(D15)。
    FocusNeighbor(Key),
    /// 重开 pane(M5b/T3):同 shell 新会话,退出态清零。
    RestartPane,
    /// 回看导出(#81):全 buffer 写 %TEMP% 文件并系统默认打开
    WriteScrollback,
    /// 滚动回看搜索(#57):开/关注入式搜索模式(窗口级,非 pane 级)
    SearchScrollback,
    /// 透明度切换(#78):1.0 ↔ background-opacity(默认 0.85)两档
    ToggleBackgroundOpacity,
    /// 重开最近关闭的标签(#88,LIFO):恢复 shell/目录/标题
    ReopenTab,
    /// 重命名当前标签(#87):进入行内改名模式
    RenameTab,
    /// OSC 133 jump(D37):跳上一个 prompt 行。
    JumpPrevPrompt,
    /// OSC 133 jump(D37):跳下一个 prompt 行;越过最新 = 回底部跟随。
    JumpNextPrompt,
}

/// 键位表:用户条目在前(优先),默认表垫底。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Keymap {
    binds: Vec<(Trigger, Action)>,
}

impl Keymap {
    /// WT 兼容默认表(D15):目标用户是 WT 迁移者,肌肉记忆零成本。
    pub fn wt_default() -> Self {
        crate::config::settings::Settings::default().keymap
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
            "win" => mods.win = true,
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
        // ` 键(美式键盘反引号;Quick Terminal 默认热键)
        "oem_3" | "grave" | "backquote" => TriggerKey::Key(Key::Grave),
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
        "new_window" => Some(Action::NewWindow),
        "close_tab" => Some(Action::CloseTab),
        "next_tab" => Some(Action::NextTab),
        "prev_tab" => Some(Action::PrevTab),
        "copy" => Some(Action::Copy),
        "paste" => Some(Action::Paste),
        "scroll_top" => Some(Action::ScrollTop),
        "scroll_bottom" => Some(Action::ScrollBottom),
        "jump_prev_prompt" => Some(Action::JumpPrevPrompt),
        "jump_next_prompt" => Some(Action::JumpNextPrompt),
        "scroll_line_up" => Some(Action::ScrollLine(1)),
        "scroll_line_down" => Some(Action::ScrollLine(-1)),
        "scroll_page_up" => Some(Action::ScrollPage(1)),
        "scroll_page_down" => Some(Action::ScrollPage(-1)),
        "split_right" => Some(Action::SplitRight),
        "split_down" => Some(Action::SplitDown),
        "close_pane" => Some(Action::ClosePane),
        "restart_pane" => Some(Action::RestartPane),
        "write_scrollback" => Some(Action::WriteScrollback),
        "search_scrollback" => Some(Action::SearchScrollback),
        "toggle_background_opacity" => Some(Action::ToggleBackgroundOpacity),
        "reopen_tab" => Some(Action::ReopenTab),
        "rename_tab" => Some(Action::RenameTab),
        "focus_left" => Some(Action::FocusNeighbor(Key::Left)),
        "focus_right" => Some(Action::FocusNeighbor(Key::Right)),
        "focus_up" => Some(Action::FocusNeighbor(Key::Up)),
        "focus_down" => Some(Action::FocusNeighbor(Key::Down)),
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
    fn external_binding_table_preserves_every_legacy_default() {
        let keymap = Keymap::wt_default();
        let expected = [
            ("ctrl+shift+t", Action::NewTab),
            ("ctrl+t", Action::NewTab),
            ("ctrl+shift+n", Action::NewWindow),
            ("ctrl+shift+w", Action::ClosePane),
            ("ctrl+tab", Action::NextTab),
            ("ctrl+shift+tab", Action::PrevTab),
            ("ctrl+shift+c", Action::Copy),
            ("ctrl+shift+v", Action::Paste),
            ("ctrl+insert", Action::Copy),
            ("shift+insert", Action::Paste),
            ("ctrl+alt+left", Action::FocusNeighbor(Key::Left)),
            ("ctrl+alt+right", Action::FocusNeighbor(Key::Right)),
            ("ctrl+alt+up", Action::FocusNeighbor(Key::Up)),
            ("ctrl+alt+down", Action::FocusNeighbor(Key::Down)),
            ("shift+pageup", Action::ScrollPage(1)),
            ("ctrl+shift+r", Action::RestartPane),
            ("ctrl+shift+s", Action::WriteScrollback),
            ("ctrl+shift+z", Action::ReopenTab),
            ("ctrl+shift+o", Action::ToggleBackgroundOpacity),
            ("ctrl+shift+f", Action::SearchScrollback),
            ("ctrl+shift+up", Action::JumpPrevPrompt),
            ("ctrl+shift+down", Action::JumpNextPrompt),
            ("shift+pagedown", Action::ScrollPage(-1)),
            ("ctrl+shift+home", Action::ScrollTop),
            ("ctrl+shift+end", Action::ScrollBottom),
        ];
        assert_eq!(keymap.binds.len(), expected.len() + 9);
        for (source, action) in expected {
            let trigger = parse_trigger(source).unwrap();
            assert_eq!(
                keymap.lookup(trigger.mods, trigger.key),
                Some(action),
                "{source}"
            );
        }
        for index in 1..=9 {
            let trigger = parse_trigger(&format!("ctrl+shift+{index}")).unwrap();
            assert_eq!(
                keymap.lookup(trigger.mods, trigger.key),
                Some(Action::GotoTab(index))
            );
        }
    }

    /// #79 重映射语义:交换语义(源↔目标),无源修饰时目标清位
    #[test]
    fn key_remap_swaps_mods() {
        let ctrl_super = KeyRemap::parse_pair("ctrl=super").unwrap();
        let base = Mods {
            shift: false,
            alt: false,
            ctrl: true,
            win: false,
        };
        let out = ctrl_super.apply(base);
        assert!(!out.ctrl && out.win, "ctrl→win(物理 ctrl 编码为 super)");
        let back = ctrl_super.apply(Mods {
            shift: false,
            alt: false,
            ctrl: false,
            win: true,
        });
        assert!(back.ctrl && !back.win, "物理 win → 编码 ctrl");
        let none = ctrl_super.apply(Mods {
            shift: false,
            alt: false,
            ctrl: false,
            win: false,
        });
        assert!(!none.ctrl && !none.win, "都没按:不变");
        assert!(KeyRemap::parse_pair("alt=meta").is_some(), "别名");
        assert!(KeyRemap::parse_pair("bad=x").is_none());
        assert!(KeyRemap::parse_pair("noeq").is_none());
    }

    #[test]
    fn wt_default_covers_core_combos() {
        let km = Keymap::wt_default();
        let m = |c: bool, s: bool, a: bool| Mods {
            shift: s,
            alt: a,
            ctrl: c,
            win: false,
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
                    alt: false,
                    win: false
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
                    alt: false,
                    win: false
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
                    alt: false,
                    win: false,
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
